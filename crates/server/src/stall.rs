//! A connection wrapper that gives up on peers that hold a connection
//! without using it.
//!
//! Two things no layer above notices:
//!
//! * A client that connects and then **sends nothing**. HTTP's own header
//!   timeout only starts once the first bytes have told the server which
//!   HTTP version it is dealing with; until then the connection would wait
//!   forever.
//! * A client that keeps its connection open but **never reads** from it.
//!   Every write then waits forever: the response is never finished, the
//!   stream body is never dropped, and the upstream request behind it stays
//!   open — as does every client WebSocket in the same state. TCP itself
//!   never times such a connection out (the peer is alive, its window is
//!   just closed).
//!
//! [`StallGuard`] fails the first read when nothing has arrived by a
//! deadline, and fails a write that has made no progress at all for a set
//! time; either ends the connection like any other I/O error. Neither is a
//! limit on how long a request or a response may take: after its first
//! byte a client may be as quiet as it likes, and every byte the peer
//! accepts starts the write clock afresh.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

/// See the module documentation.
pub(crate) struct StallGuard<S> {
    inner: S,
    write_timeout: Duration,
    /// Armed while a write is waiting for the peer.
    stalled_since: Option<Pin<Box<Sleep>>>,
    /// Until the peer's first byte: when to stop waiting for it.
    silent_until: Option<Pin<Box<Sleep>>>,
}

impl<S> StallGuard<S> {
    /// Wraps `inner`. The peer has `first_byte` to send anything at all; a
    /// write blocked for `write_timeout` fails. Both failures are
    /// [`io::ErrorKind::TimedOut`].
    pub(crate) fn new(inner: S, first_byte: Duration, write_timeout: Duration) -> Self {
        StallGuard {
            inner,
            write_timeout,
            stalled_since: None,
            silent_until: Some(Box::pin(tokio::time::sleep(first_byte))),
        }
    }

    /// Applies the stall rule to the outcome of a write-side operation.
    fn watch<T>(
        &mut self,
        cx: &mut Context<'_>,
        outcome: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        match outcome {
            Poll::Ready(result) => {
                // Progress (or a real error): the clock stops.
                self.stalled_since = None;
                Poll::Ready(result)
            }
            Poll::Pending => {
                let timeout = self.write_timeout;
                let timer = self
                    .stalled_since
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
                if timer.as_mut().poll(cx).is_ready() {
                    self.stalled_since = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the peer accepted no data for too long",
                    )));
                }
                Poll::Pending
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for StallGuard<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(result) => {
                if buf.filled().len() > before {
                    // The peer has spoken; from here on, quiet is allowed.
                    self.silent_until = None;
                }
                Poll::Ready(result)
            }
            Poll::Pending => {
                if let Some(deadline) = self.silent_until.as_mut()
                    && deadline.as_mut().poll(cx).is_ready()
                {
                    self.silent_until = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the peer connected and sent nothing",
                    )));
                }
                Poll::Pending
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for StallGuard<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let outcome = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.watch(cx, outcome)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let outcome = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.watch(cx, outcome)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let outcome = Pin::new(&mut self.inner).poll_flush(cx);
        self.watch(cx, outcome)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let outcome = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.watch(cx, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FIRST_BYTE: Duration = Duration::from_secs(120);
    const TIMEOUT: Duration = Duration::from_secs(60);

    fn guard<S>(inner: S) -> StallGuard<S> {
        StallGuard::new(inner, FIRST_BYTE, TIMEOUT)
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_nobody_reads_times_out() {
        // The pipe holds 16 bytes; nobody reads the other end.
        let (ours, _theirs) = tokio::io::duplex(16);
        let mut guarded = guard(ours);
        let started = tokio::time::Instant::now();
        let error = guarded
            .write_all(&[0u8; 64])
            .await
            .expect_err("the write must give up");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_is_not_a_stalled_one() {
        let (ours, mut theirs) = tokio::io::duplex(16);
        let mut guarded = guard(ours);
        // Reads 16 bytes every 45 seconds: far slower than anyone would
        // like, but never a whole timeout without progress.
        let reader = tokio::spawn(async move {
            let mut total = 0;
            let mut chunk = [0u8; 16];
            while total < 160 {
                tokio::time::sleep(Duration::from_secs(45)).await;
                total += theirs.read(&mut chunk).await.unwrap();
            }
            total
        });
        guarded.write_all(&[7u8; 160]).await.unwrap();
        guarded.flush().await.unwrap();
        assert_eq!(reader.await.unwrap(), 160);
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_restarts_after_progress() {
        let (ours, mut theirs) = tokio::io::duplex(16);
        let mut guarded = guard(ours);
        // The reader takes one helping after 50 s and then stops for good.
        let reader = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(50)).await;
            let mut chunk = [0u8; 16];
            theirs.read_exact(&mut chunk).await.unwrap();
            // Keep the pipe open without reading.
            tokio::time::sleep(Duration::from_secs(600)).await;
            drop(theirs);
        });
        let started = tokio::time::Instant::now();
        let error = guarded
            .write_all(&[0u8; 64])
            .await
            .expect_err("the write must give up");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // 50 s until the first progress, then a full timeout from there.
        assert_eq!(started.elapsed(), Duration::from_secs(50) + TIMEOUT);
        reader.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_speaks_is_dropped() {
        let (ours, _theirs) = tokio::io::duplex(64);
        let mut guarded = guard(ours);
        let started = tokio::time::Instant::now();
        let mut byte = [0u8; 1];
        let error = guarded
            .read(&mut byte)
            .await
            .expect_err("the read must give up");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), FIRST_BYTE);
    }

    #[tokio::test(start_paused = true)]
    async fn after_its_first_byte_a_peer_may_be_quiet() {
        let (ours, mut theirs) = tokio::io::duplex(64);
        let mut guarded = guard(ours);
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(100)).await;
            theirs.write_all(b"G").await.unwrap();
            // Ten times the first-byte allowance of silence, then more.
            tokio::time::sleep(FIRST_BYTE * 10).await;
            theirs.write_all(b"E").await.unwrap();
            theirs
        });
        let mut got = [0u8; 2];
        guarded.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"GE");
        drop(writer.await.unwrap());
    }

    #[tokio::test]
    async fn reads_and_writes_pass_through() {
        let (ours, mut theirs) = tokio::io::duplex(1024);
        let mut guarded = guard(ours);
        guarded.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        theirs.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        theirs.write_all(b"pong").await.unwrap();
        guarded.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"pong");
        guarded.shutdown().await.unwrap();
    }
}
